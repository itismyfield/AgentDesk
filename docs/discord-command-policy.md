# Discord command policy

Slash commands carry a risk tier, checked before the handler runs. This is
separate from the intake authentication/admission checks, which still apply.

| Risk | Permission contract |
| --- | --- |
| ReadOnly | Existing authorized users |
| Mutating | Existing authorized users; not globally owner-only |
| ShellOrToolGrant | Bot owner and the explicit-enable setting (no command uses this tier today) |
| CredentialSystem | Bot owner; no additional explicit-enable requirement |

## Agent text commands

Agents cannot run slash commands, so three session controls stay as plain
messages, mainly for the announce bot that agent-to-agent sends post as:

- `!clear` runs the `/clear` handler.
- `!restart` runs the `/restart` handler.
- `!stop` runs the native stop path `/stop` shares.

A message is a text command only when its first word, after an optional leading
bot mention, is exactly one of these names. The other `!` commands were removed;
a message whose first word is a removed name gets a one-line answer naming the
slash command to use instead, and runs nothing. Any other `!` text is ordinary
chat. The owner-only `!herdr consent retry` is the Herdr consent flow's own
command and keeps its exact form.
