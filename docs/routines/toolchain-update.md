# External toolchain update routine

Issue #4555 adds a weekly, draft-first inventory check for the external tools used by AgentDesk.
The scheduler never applies an update. Its only entry point is:

```bash
python3 scripts/toolchain_update.py check
```

That command performs bounded, read-only current/latest probes and atomically rewrites
`runtime/toolchain-update/latest.md` plus its machine-readable sibling `latest.json`. Every
inventory target always receives a row. Missing binaries, registry failures, unreachable remote
services, and manual-only tools are reported explicitly rather than silently dropped.

## Inventory and authority

| Update method | Targets | Authority |
|---|---|---|
| native installer | `claude` | approval required; never scheduled for apply |
| npm global | `codex`, `ocx`, `claude-e` | `codex`/`ocx` approval tier; all npm mutations, including hygiene `claude-e`, require an exact per-tool approval marker |
| uv tool | `cswap` (`claude-swap`) | approval required because `--list --json` has drifted |
| rustup | `cargo/rustc` | hygiene candidate; per-tool approval required; rustup binary self-update disabled; post-update compiler smoke required |
| Homebrew | `tmux`, `gh`, `node`, `python@3.14`, `uv`, `pipx`, `jq`, `ripgrep`, `ffmpeg`, `whisper-cpp`, `postgresql@17` | `tmux`, `node`, and PostgreSQL require approval; the rest are hygiene candidates |
| pipx | `edge-tts` | hygiene candidate |
| native installer | `opencode` | hygiene candidate only when already installed; its self-updater still requires exact per-tool approval |
| remote service | memento MCP at the configured mac-mini endpoint | health/version report only; no local mutation |
| npx always-latest | brave-search MCP | reports the unpinned spec and latest package version; pin approval is a manual config decision |
| manual/receipt | `SidecarLauncher`, Playwright + Chromium | report only; no mutation |

The `check --offline` diagnostic skips all registry and HTTP probes, including memento, while still
emitting every row. Normal weekly checks do query npm, PyPI, Homebrew metadata, rustup, and memento.
Homebrew metadata probes set `HOMEBREW_NO_AUTO_UPDATE=1`.

## Draft to approval to apply

Review `latest.md`, especially the approval cards, risks, and changelog links. Each generated draft
has a fresh nonce included in its SHA-256 draft ID. An approval marker is bound to that exact draft
instance and exact current/latest pair, so editing, replacing, or regenerating byte-identical checks
invalidates existing approval.

```bash
ROOT="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"
DRAFT="$ROOT/runtime/toolchain-update/latest.json"

python3 "$ROOT/scripts/toolchain_update.py" approve \
  --draft "$DRAFT" \
  --tool codex \
  --confirm approve-exact-toolchain-draft
```

Applying is a separate human invocation. It requires the literal safe-window confirmation and then
independently fails closed unless `agentdesk status --json` reports zero working/active-dispatch
sessions, the queue has no non-terminal live status, neither `cargo` nor `rustc` is running, and no
`deploy-release.sh` process exists. Current/latest versions are re-probed before mutation; any
mismatch with the approved draft is treated as a stale draft and blocks apply. The full safe-window
check runs again immediately before every destructive command to close the preflight-to-mutation
race.

```bash
python3 "$ROOT/scripts/toolchain_update.py" apply \
  --draft "$DRAFT" \
  --tool codex \
  --confirm-safe-window no-active-turns-or-deploys
```

`--apply-hygiene` selects every hygiene row whose draft decision is `update-available`.
Native/self-updater/rustup/npm mutations remain approval-gated even when their tier is hygiene, so
`claude-e`, `cargo/rustc`, and `opencode` must be approved first. Rustup toolchain updates pass
`--no-self-update`, so they cannot silently replace the rustup binary. Report-only rows cannot enter
the apply path. The launchd plist contains neither `approve` nor `apply` and cannot cross either gate.

## Smoke and failure handling

Each applied tool must first re-probe at exactly the draft's approved target version, then
immediately runs its profile before the batch can continue. A newer-at-runtime native updater
result is an unapproved version and fails before smoke:

- `claude --version`, `codex --version`, and other version probes must contain a semantic version;
  Codex output uses the highest semantic version found.
- `cswap --list --json` must match the permissive camelCase object/list shape consumed by
  `src/services/cswap.rs`, including fractional `usageAgeSeconds`.
- `ocx health` must succeed after the version probe. This catches the proxy-stop behavior observed
  during prior `ocx update` use; the script does not restart the proxy automatically.
- `tmux -V` must parse; Node must load `npm ls -g --depth=0 --json`, retain every npm-global CLI in
  the manifest (`codex`, `ocx`, `claude-e`), and successfully probe every corresponding binary.
- PostgreSQL requires both `psql --version` and a matching server major from `SHOW server_version`.
  A strict apply therefore requires `AGENTDESK_DATABASE_URL` or `DATABASE_URL`.
- Rust checks the actual rustup `stable` rustc/cargo pair; Homebrew hygiene tools, `edge-tts`, and
  `opencode` must load their version command.

The smoke gate can also be run independently:

```bash
python3 "$ROOT/scripts/toolchain_update.py" smoke --tool codex --tool cswap
```

On the first failed update command, exact-version verification, or post-update smoke, the batch
stops and exits non-zero. Build-capable Homebrew updates receive a two-hour timeout rather than
being killed after five minutes. Every post-mutation failure writes
`runtime/toolchain-update/alerts/<draft>-<tool>.md` with the failed checks and an exact rollback
command where package managers support version pins, or a Homebrew pin/native rollback instruction
otherwise. No dcserver/proxy restart occurs until an operator completes that plan and the smoke
profile passes.

## launchd template (documented installation only)

`scripts/launchd/com.agentdesk.toolchain-update.plist` runs every Sunday at 09:30 in the host's
local timezone. It is a source template: replace `__AGENTDESK_ROOT__` with the absolute release
root and `__HOME__` with the user's absolute home directory before loading. Its explicit `PATH`
includes user-local, rustup, opencode, and `~/bin` locations before Homebrew and system directories,
so scheduled probes can find the same managed tools. The repository does not install or load it
automatically.

The following are operator commands; implementation and tests do not execute them:

```bash
ROOT="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"
SOURCE="$ROOT/scripts/launchd/com.agentdesk.toolchain-update.plist"
DEST="$HOME/Library/LaunchAgents/com.agentdesk.toolchain-update.plist"

sed -e "s|__AGENTDESK_ROOT__|$ROOT|g" -e "s|__HOME__|$HOME|g" "$SOURCE" > "$DEST"
plutil -lint "$DEST"
launchctl bootstrap "gui/$(id -u)" "$DEST"
```

To remove the schedule, use `launchctl bootout "gui/$(id -u)/com.agentdesk.toolchain-update"` and
then remove the materialized plist. Removing the schedule does not remove drafts or approval
markers.
