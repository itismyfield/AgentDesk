use super::super::{Context, Error};
use crate::services::provider::ProviderKind;

/// Build the main `/help` body. Pulled out so we can unit-test the size budget
/// — Discord rejects single messages over 2000 characters, and codex review
/// (issue #1005 PR follow-up) caught that adding the risk block inline tipped
/// the longest provider variant just past that limit.
fn build_main_help_body(provider: &ProviderKind) -> String {
    let provider_name = provider.display_name();
    let model_section = match provider {
        ProviderKind::Codex => {
            "\n`/model` — Open the model picker for this channel\n`/fast` — Toggle native fast mode for this channel\n`/goals` — Toggle Codex goals for this channel"
        }
        ProviderKind::Claude => {
            "\n`/model` — Open the model picker for this channel\n`/fast` — Toggle native fast mode for this channel\n`/effort <level>` — Claude effort (`low`…`max`)\n`/compact`, `/cost`, `/context` — Claude session commands"
        }
        _ => "\n`/model` — Open the model picker for this channel",
    };
    format!(
        "\
**AgentDesk Discord Bot**
Manage files and chat with {}.
Each channel gets an independent session.

**Session**
`/start <path>` — Start session at directory
`/start` — Start with auto-generated workspace
`/pwd` — Show current working directory
`/node` — Pick the cluster node for this channel
`/health` — Show runtime health summary
`/status` — Show this channel session status
`/inflight` — Show saved inflight turn state
`/clear` — Clear AI conversation history
`/stop` — Stop current AI request
`/cancel-queued <message_id>` — Remove one queued message (`/queue` shows IDs)

**File Transfer**
`/down <file>` — Download file from server
Send a file/photo — Upload to session directory

**Shell**
`!<command>` — Run shell command directly
`/shell <command>` — Run shell command (slash command)

**AI Chat**
Any other message is sent to {}.
AI can read, edit, and run commands in your session.

**Tool Management** (Qwen only)
`/allowedtools` — Show currently allowed tools
`/allowed +name` — Add tool (e.g. `/allowed +Bash`)
`/allowed -name` — Remove tool

**Analytics**
`/usage [ratelimit|month]` — Text token/rate-limit summary
`/receipt [month|ratelimit]` — Same usage data as PNG
`/metrics [date]` — Local turn metrics by date/channel

**Skills**
`/skill <skill>` — Run a provider skill (autocomplete)
`/cc <skill>` — Legacy alias for `/skill`

**Restart**
`/restart` — Restart this provider session (resumes when supported)

**Settings**
{}
`/debug` — Toggle debug logging

**User Management** (owner only)
`/allowall <true|false>` — Allow everyone or restrict to authorized users
`/adduser @user` — Allow a user to use the bot
`/removeuser @user` — Remove a user's access
`/help` — Show this help

(Command risk tiers follow.)",
        provider_name, provider_name, model_section
    )
}

/// /help — Show help information
#[cfg(test)]
mod tests {
    use super::build_main_help_body;
    use crate::services::provider::ProviderKind;

    #[test]
    fn help_advertises_explicit_queue_cancellation_not_reaction_removal() {
        let body = build_main_help_body(&ProviderKind::Claude);
        assert!(body.contains("`/cancel-queued <message_id>`"));
        assert!(body.contains("`/queue` shows IDs"));
        assert!(!body.contains("reaction remove"));
    }
}

#[poise::command(slash_command, rename = "help")]
pub(in crate::services::discord) async fn cmd_help(ctx: Context<'_>) -> Result<(), Error> {
    let help = build_main_help_body(&ctx.data().provider);
    ctx.say(help).await?;
    // Issue #1005: surface command risk tiers + the high-risk opt-in state in
    // a follow-up message. Splitting the response keeps each chunk well below
    // Discord's 2000-character limit (combined the longest provider variant
    // overran by ~45 chars) and makes the risk surface easy to copy/paste.
    let risk_block = super::risk_tier_summary_for_help(super::high_risk_enabled_via_env());
    ctx.say(risk_block).await?;
    Ok(())
}
