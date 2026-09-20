# CLI session anchors

`scripts/session-anchor.py` and `scripts/session-anchor.zsh` are the canonical
sources for the personal `cc`, `cct`, `cdx`, and `cdxt` launchers. Install both as
regular files in `~/.config/agentdesk/`; `.zshrc` sources the installed `.zsh` file.
Do not symlink them to a disposable development worktree. Updating the AgentDesk
binary does not update these personal shell files: when these sources change,
refresh both installed copies from the reviewed checkout before opening a new
shell. Python 3.11 or later is required; this Mac uses Homebrew Python.

Mac mini uses `scripts/session-anchor-mac-mini.zsh` as the installed
`~/.config/agentdesk/session-anchor.zsh` to preserve that host's 1M compact window
and bare Codex alias. The Python helper is identical on both machines. Refresh
the matching host variant when updating personal shell files. For its regression
tests set `SESSION_ANCHOR_TEST_ZSHRC` to that variant and
`SESSION_ANCHOR_TEST_COMPACT_DEFAULT=1000000`.

On a new CLI process the helper reads the configured Memento MCP endpoint and
credential environment variable, initializes MCP, and reads `context`. Only the
complete `[ANCHOR MEMORY]` section is appended. Claude uses
`--append-system-prompt`; Codex uses `developer_instructions`. Existing custom
system prompts, appended instructions, model/permission options, and user prompts
are preserved. Codex user/profile/trusted-project developer instructions are
merged before the anchor addition. Built-in system instructions are not replaced.

Scope resolution is `MEMENTO_WORKSPACE`, then the canonical Git repository name
using `--git-common-dir` (including worktrees), normalized to lowercase ASCII
letters/digits and hyphen separators (`AgentDesk` becomes `agentdesk`). Explicit
`MEMENTO_WORKSPACE` values are preserved verbatim. Codex `-C`/`--cd` is respected.
Outside a repository, context's key-default workspace is **not** assumed global:
an additional `recall(isAnchor=true, excludeSeen=false)` requests explicit workspace
metadata, verifies its count against context, and retains only `workspace:null`
fragments. Missing metadata/count mismatches fail open without injecting a partial
or mixed scope. `MEMENTO_MCP_URL` can explicitly override the configured endpoint.

An existing tmux session is only attached. Resume/continue/fork use saved provider
instructions without fetching or retrofitting anchors, with a short notice.
Therefore old sessions created before this change do not gain anchors; start a
new process for that. `/clear` and compaction within a newly launched process use
the process's persistent instruction setting. They do not trigger a fresh MCP
read. This design establishes the persistent CLI instruction input; live model
behavior across compaction has not been tested with a paid model request.

Help, version, and management commands do not fetch anchors. MCP/config errors,
timeouts, empty results, and malformed/oversize anchor sections leave the original
invocation intact. Diagnostics contain neither credentials nor response content.

Run offline regression checks with:

```sh
/opt/homebrew/bin/python3 -m unittest discover -s scripts/__tests__ -p test_session_anchor.py -v
zsh -n scripts/session-anchor.zsh
```

Tests use fake Claude/Codex/tmux executables and a local fake MCP server. They cover
all four launch paths, command escaping, configuration preservation, worktree and
global isolation, resume/attach, failures, and explicit compact-window settings.
`SESSION_ANCHOR_TEST_ZSHRC` can select a wrapper source to test another installed
copy. No LLM request is made.

Codex configuration source: [official sample configuration](https://learn.chatgpt.com/docs/config-file/config-sample)
and [configuration precedence](https://learn.chatgpt.com/docs/config-file/config-basic).
The installed Codex `--help` and Claude `--help` confirm the supported CLI flags.
