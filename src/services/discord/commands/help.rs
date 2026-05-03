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
            "\n`/model` — Open the model picker for this channel\n`/fast` — Toggle native fast mode for this channel"
        }
        _ => "\n`/model` — Open the model picker for this channel",
    };
    format!(
        "\
**AgentDesk Discord Bot**
Manage server files & chat with {}.
Each channel gets its own independent {} session.

**Session**
`/start <path>` — Start session at directory
`/start` — Start with auto-generated workspace
`/pwd` — Show current working directory
`/health` — Show runtime health summary
`/status` — Show this channel session status
`/inflight` — Show saved inflight turn state
`/clear` — Clear AI conversation history
`/stop` — Stop current AI request

**File Transfer**
`/down <file>` — Download file from server
Send a file/photo — Upload to session directory

**Shell**
`!<command>` — Run shell command directly
`/shell <command>` — Run shell command (slash command)

**AI Chat**
Any other message is sent to {}.
AI can read, edit, and run commands in your session.

**Tool Management** (Qwen only — Claude exposes all tools by default)
`/allowedtools` — Show currently allowed tools
`/allowed +name` — Add tool (e.g. `/allowed +Bash`)
`/allowed -name` — Remove tool

**Analytics**
`/receipt [month|ratelimit]` — Show token usage receipt (PNG)

**Skills**
`/cc <skill>` — Run a provider skill (autocomplete)

**Restart**
`/restart` — Restart this provider session immediately (preserves conversation via --resume when supported)

**Settings**
{}
`/debug` — Toggle debug logging

**User Management** (owner only)
`/allowall <true|false>` — Allow everyone or restrict to authorized users
`/adduser @user` — Allow a user to use the bot
`/removeuser @user` — Remove a user's access
`/help` — Show this help

(See follow-up message for command risk tiers — issue #1005.)",
        provider_name, provider_name, provider_name, model_section
    )
}

/// /help — Show help information
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    /// Discord rejects single messages over 2000 characters. Pin every provider
    /// variant of `/help` against the limit so future edits cannot regress it.
    /// We assert against 1900 to leave a buffer for unicode-heavy expansions.
    #[test]
    fn main_help_body_stays_under_discord_message_limit() {
        const SAFE_LIMIT: usize = 1900;
        for provider in [
            ProviderKind::Claude,
            ProviderKind::Codex,
            ProviderKind::Qwen,
            ProviderKind::Gemini,
        ] {
            let body = build_main_help_body(&provider);
            assert!(
                body.chars().count() < SAFE_LIMIT,
                "/help body for {provider:?} is {} chars (limit {SAFE_LIMIT})",
                body.chars().count(),
            );
        }
    }

    /// Risk block is sent as a separate message, but it must also stay under
    /// the Discord cap on its own.
    #[test]
    fn risk_summary_stays_under_discord_message_limit() {
        const SAFE_LIMIT: usize = 1900;
        for enabled in [false, true] {
            let block = super::super::risk_tier_summary_for_help(enabled);
            assert!(
                block.chars().count() < SAFE_LIMIT,
                "risk block (enabled={enabled}) is {} chars (limit {SAFE_LIMIT})",
                block.chars().count(),
            );
        }
    }
}
