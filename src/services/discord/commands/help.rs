use super::super::{Context, Error};

/// /help — Show help information
#[poise::command(slash_command, rename = "help")]
pub(in crate::services::discord) async fn cmd_help(ctx: Context<'_>) -> Result<(), Error> {
    let provider_name = ctx.data().provider.display_name();
    let help = format!(
        "\
**AgentDesk Discord Bot**
Manage server files & chat with {}.
Each channel gets its own independent {} session.

**Session**
`/start <path> [remote]` — Start session at directory
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

**Tool Management**
`/allowedtools` — Show currently allowed tools
`/allowed +name` — Add tool (e.g. `/allowed +Bash`)
`/allowed -name` — Remove tool

**Analytics**
`/receipt [month|ratelimit]` — Show token usage receipt (PNG)

**Skills**
`/cc <skill>` — Run a provider skill (autocomplete)

**Settings**
`/debug` — Toggle debug logging

**User Management** (owner only)
`/allowall <true|false>` — Allow everyone or restrict to authorized users
`/adduser @user` — Allow a user to use the bot
`/removeuser @user` — Remove a user's access
`/help` — Show this help",
        provider_name, provider_name, provider_name
    );

    ctx.say(help).await?;
    Ok(())
}
