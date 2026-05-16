# Codex Remote SSH Runtime Policy

Issue: #2193
Status: Proposed

This ADR fixes the policy gap that #2193 opened: AgentDesk's
`execute_streaming_remote_direct` and `execute_streaming_remote_tmux` are
currently disabled stubs that return errors, and #2175 classified them as
"not allowed now" without saying what would have to be true before they
flip on. This document is that contract. Until every prerequisite below is
satisfied, remote Codex over SSH stays off, and the codepath stays a stub.

This ADR pairs with `docs/codex-exec-policy.md` (#2175). That ADR defines
which local Codex paths may speak `codex exec --json`. This ADR defines
when, how, and under whose authority a Codex turn is allowed to leave the
AgentDesk host at all.

## Context

AgentDesk hosts Codex via three local strategies — direct TUI hosting,
the legacy tmux wrapper, and ProcessBackend — all of which run Codex as a
child of the AgentDesk process on the same host. `RemoteProfile` exists in
the codebase (`src/services/remote_stub.rs`) and is plumbed through
`execute_command_streaming`, but `services::remote` is a stub: SSH is
unavailable in the AgentDesk build, `ssh_connect_and_auth` returns `Err`,
and the two remote entry points always error.

The gap remote SSH would fill is operator-driven: running a Codex turn
*as* a different machine (typically `mac-mini` or `mac-book`) when that
host owns the codebase under edit, the build toolchain, or the
project-specific MCP servers. The pull is real, but the current code
plumbs a profile shape (`RemoteAuth::Password { password }`,
`RemoteAuth::KeyFile { path, passphrase }`) that, if turned on as-is,
would put cleartext secrets in agentdesk.yaml, fan out to every
configured profile, and have no operator-visible cancel path. That is the
ambiguity this ADR closes.

## Decision

Remote Codex execution over SSH is a **single-policy** feature, not a
configurable matrix. When it is eventually enabled, it MUST conform to
every clause below. Until it does, both `execute_streaming_remote_direct`
and `execute_streaming_remote_tmux` remain stubs that return
`Err("Remote SSH execution is not available in AgentDesk")`, and the
top-level dispatcher MUST refuse to route a turn to them when the gate is
off.

The gate is a single boolean: `providers.codex.remote_ssh_enabled` in
`agentdesk.yaml`, default `false`. Setting it to `true` is a declaration
by the operator that every prerequisite in this ADR is in place. The gate
is intentionally codex-scoped and not a top-level toggle; other providers
(Claude, Gemini, Qwen, OpenCode) have their own remote stubs and will get
their own ADRs if and when they need them.

This policy intentionally picks one model rather than enumerating
options. The model is: **AgentDesk SSHes to a small, operator-curated
allow-list of hosts it already owns, using the operator's existing
ssh-agent identity, and runs Codex there as a non-interactive child whose
lifecycle is bound 1:1 to a local cancellation token.**

## Trust boundary

The trust boundary sits at the AgentDesk process. Everything inside the
AgentDesk process is trusted: configuration, secrets, dispatcher state,
the Codex prompt, the discord control plane. Everything on the far side
of an SSH session is **outside** the boundary and is treated as a remote
execution surface, not a remote data store.

Concretely, the following are allowed to cross the boundary into the
remote shell:

- The composed Codex prompt for the current turn (system prompt +
  allowed-tools + user prompt) — same payload the local Codex child
  would see.
- The working directory path on the remote host (operator-supplied per
  profile).
- Codex CLI arguments (model, reasoning effort, readonly flag,
  compact-token-limit, fast-mode, goals).
- A short-lived correlation id used only for log stitching.

The following are **never** allowed to cross the boundary:

- AgentDesk's Discord bot token, database credentials, GitHub PAT,
  provider API keys for other providers, MCP credential files, or any
  member of `BotConfig` / `DiscordBotAuthConfig`.
- The AgentDesk config file in any form, including via env-var
  smuggling.
- Cancel tokens, dispatcher handles, or any in-process object — only
  the *effect* of a cancel crosses, as a signal (see "Cancel
  semantics").
- Codex rollout transcripts from other sessions. The remote turn writes
  its own rollout to the remote host; AgentDesk reads only the JSONL
  stream of the in-flight turn back over the SSH stdout channel.

The remote host is assumed to be **operator-controlled** (their
`mac-mini`, `mac-book`, or a CI runner they administer). It is not
assumed to be hardened against the operator. A compromised remote host
is treated under "Failure / blast radius" below.

## Auth

AgentDesk authenticates to the remote host using the operator's
**ssh-agent** identities, not credentials embedded in `agentdesk.yaml`.

- The shipping implementation MUST consume `SSH_AUTH_SOCK` and offer
  agent-resident identities to the remote `sshd`. It MUST NOT read
  `~/.ssh/id_*` private key files directly, and it MUST NOT accept a
  passphrase or password from `RemoteProfile`.
- `RemoteAuth::Password { password }` and `RemoteAuth::KeyFile { path,
  passphrase }` in `remote_stub.rs` are **deprecated by this ADR**.
  When remote SSH is enabled, those variants MUST be rejected at
  config-load time with a clear error pointing at this document. The
  enum variants stay in the source for backwards-compat parse
  tolerance, but they cannot authenticate.
- The remote Codex child does **not** authenticate back to AgentDesk.
  It writes to its stdout/stderr (the JSONL stream and incidental
  logs), and that stream is consumed inside the existing SSH channel.
  There is no second inbound connection from the remote host to the
  AgentDesk host. Removing reverse auth is what keeps the trust
  boundary one-directional.
- Known-host pinning is mandatory. AgentDesk MUST verify the remote
  host key against `~/.ssh/known_hosts` (strict). A missing or changed
  host key is a hard failure; there is no TOFU prompt and no
  auto-accept.

The chosen model is "use the operator's ssh-agent" rather than
"AgentDesk holds its own key" because the operator already has
ssh-agent + Touch ID / 1Password on `mac-mini` and `mac-book`. Adding a
service-account key inside AgentDesk would create a new long-lived
credential surface for no functional gain.

### SSH invocation hardening

If the implementation ships subprocess `ssh` (the leaning option in
"Open questions"), the user's `~/.ssh/config` MUST NOT be consulted,
because a stanza in user config can turn a single allow-listed host
into a multi-hop route (`ProxyJump`/`ProxyCommand`), swap the
authentication source from ssh-agent to an arbitrary on-disk key
(`IdentityFile`), or weaken host-key pinning
(`StrictHostKeyChecking=no`, `UserKnownHostsFile=/dev/null`). The
allow-list naming a host name MUST be authoritative over the actual
endpoint and authentication behavior.

The invocation MUST be the following canonical, fully-pinned shape.
This is one tested command, not a menu — implementers MUST emit
exactly these flags in this order, and the integration test MUST
assert the rendered argv matches:

```
ssh \
  -F /dev/null \
  -o BatchMode=yes \
  -o RequestTTY=force \
  -o PasswordAuthentication=no \
  -o KbdInteractiveAuthentication=no \
  -o ChallengeResponseAuthentication=no \
  -o GSSAPIAuthentication=no \
  -o HostbasedAuthentication=no \
  -o PubkeyAuthentication=yes \
  -o IdentityFile=none \
  -o IdentitiesOnly=yes \
  -o IdentityAgent="$SSH_AUTH_SOCK" \
  -o ProxyCommand=none \
  -o ProxyJump=none \
  -o ForwardAgent=no \
  -o ForwardX11=no \
  -o ControlMaster=no \
  -o ControlPath=none \
  -o StrictHostKeyChecking=yes \
  -o UserKnownHostsFile=~/.ssh/known_hosts \
  -o ServerAliveInterval=30 \
  -o ServerAliveCountMax=2 \
  -p <port> <user>@<host> -- <remote-wrapper-invocation>
```

Rationale for the parts that are easy to get subtly wrong:

- `-F /dev/null` excludes `~/.ssh/config` and `/etc/ssh/ssh_config`
  entirely. Allow-list naming is authoritative; user config cannot
  retarget, multi-hop, or relax host-key checking.
- The four `*Authentication=no` lines plus `PubkeyAuthentication=yes`
  reduce the auth mechanism set to exactly one: public key.
- `IdentityFile=none` plus `IdentitiesOnly=yes` plus `IdentityAgent`
  together mean OpenSSH offers only the identities the running
  `SSH_AUTH_SOCK` agent holds. `IdentityFile=none` on its own is not
  enough — without `IdentitiesOnly=yes`, OpenSSH may still offer
  default on-disk identities (`~/.ssh/id_ed25519`, `~/.ssh/id_rsa`,
  etc.). The pair is the contract.
- `IdentityAgent="$SSH_AUTH_SOCK"` is included even though it
  duplicates the default, because `-F /dev/null` could otherwise be
  read as "no agent socket either". Explicitly naming the socket
  prevents that mis-reading.
- `ServerAliveInterval`/`ServerAliveCountMax` bound how long a dead
  network can keep a remote turn alive after AgentDesk has lost
  contact, which is part of the "blast radius" guarantee.

The set of agent-resident identities offered is whatever the operator
has loaded into their ssh-agent. AgentDesk does not implement a second
allow-list of *which* agent keys may be used; the operator's agent IS
the credential store, and the host allow-list is the policy surface.

Any `RemoteProfile` (or future `remote_hosts` entry) that attempts to
override one of these options is a config-load error. A regression
test MUST cover the rejection of profiles that set `ProxyJump`,
`IdentityFile`, agent forwarding, a relaxed `StrictHostKeyChecking`
value, or any `*Authentication=yes` value other than `Pubkey`.

If the implementation ships `russh` instead, the equivalent guarantees
apply: do not read `~/.ssh/config`, accept identities only from the
agent socket, do not open additional channels for forwarding, and pin
the host key against `~/.ssh/known_hosts` with strict semantics. The
hardening list above is the canonical contract; the transport choice is
implementation detail.

## Authz

Targeting a remote host is a two-step authorization, both of which MUST
hold:

1. The host MUST appear in an explicit allow-list rooted at
   `providers.codex.remote_hosts: [...]` in `agentdesk.yaml`. Each entry
   is a structured record `{ name, host, port, user, default_path }`.
   Wildcards are not accepted. The legacy free-form `remote_profiles`
   list (currently stubbed to empty in `config::Settings::load`) is
   **not** the allow-list; it is a separate compatibility shim and MUST
   NOT be consulted for routing decisions.
2. The current caller MUST be permitted to target that named host. For
   the initial rollout, the permission rule is: only agents whose
   `AgentDef` declares an explicit `codex.remote_host: <name>` field
   may dispatch a Codex turn against that name, and the name MUST match
   an entry from step 1. There is no implicit fallback to a "default"
   host, and there is no PMD-mediated dynamic targeting in this
   revision.

If either check fails, the dispatcher MUST refuse the turn before any
SSH connection is attempted, log the refusal with the agent id and the
requested host name, and return the same `Err` shape the current stub
returns so downstream UI is unchanged.

## Cancel semantics

A local AgentDesk cancel MUST tear down the remote Codex child **and
every descendant it spawned** within the same bound as a local cancel.
This section picks one process-group model rather than mixing PTY
foreground signaling with `setsid`, because those two contradict each
other: a process detached via `setsid` is no longer in the PTY's
foreground process group and PTY close / SIGHUP stops being
authoritative on it.

The contract — process-group based, not PTY-foreground based:

- AgentDesk MUST request a PTY on the SSH channel (`RequestTTY=force`
  in the invocation list above). The PTY exists so the remote shell
  has a controlling terminal that produces SIGHUP on channel close,
  and so Codex's TUI-style output flushes; it is not the primary
  cancel mechanism.
- The remote shell MUST launch Codex through a small operator-supplied
  wrapper script whose contract is:
  1. The wrapper MUST make itself the leader of a new process group
     while staying attached to the SSH-allocated PTY. Concretely:
     `set -m` (job control) plus an immediate `kill -0 -$$` self-check
     that the wrapper's PID equals its PGID; if the equality does not
     hold, the wrapper MUST `exec setsid --ctty -w bash "$0" "$@"`
     (Linux) or the equivalent `setsid` invocation that keeps the
     controlling terminal, and re-verify. The wrapper does NOT detach
     from the PTY — it only ensures `PID == PGID` so that the
     "process group" cancel target is unambiguous.
  2. The wrapper traps `SIGHUP`, `SIGINT`, and `SIGTERM`. On any of
     them it MUST send `SIGTERM` to its own process group using
     `kill -TERM 0` (which signals "every process in my process
     group", and is well-defined regardless of whether `$$` equals
     the PGID). It waits a short grace period (≤ 2s) and then sends
     `kill -KILL 0`. The shell-pid form (`kill -- -$$`) is **forbidden**
     because it assumes the wrapper is the group leader without
     verifying it; `kill 0` is the canonical correct form.
  3. The wrapper waits on Codex and exits with Codex's status. It does
     **not** background Codex, does **not** call `nohup`, and does
     **not** redirect stdout/stderr.
  4. The wrapper writes its own PID and PGID into a per-turn file the
     operator names (so the integration test can verify the group
     leader identity from outside). The file is unlinked on exit.
- On `CancelToken` fire, AgentDesk MUST, in order: (a) write `0x03`
  (ETX / Ctrl-C) into the PTY so the foreground process group sees
  SIGINT — this is Codex's chance to flush; (b) close the SSH channel,
  which produces SIGHUP for the wrapper via the PTY hangup; (c) close
  the SSH session, which is the hard-stop for the host-down /
  shell-unresponsive case.
- AgentDesk MUST NOT rely on running `kill` against a remote PID
  resolved out of band. PIDs are not stable across the trust boundary
  and the SSH user may not have permission to signal arbitrary
  processes. Cancellation is signal-only, through the PTY and channel.
- The cancel path MUST be exercised by an integration test that
  asserts **all** of the following before declaring success:
  1. The local dispatcher returns within the existing local-cancel SLO.
  2. After the cancel, the remote host has no Codex descendant alive
     in the wrapper's process group. The test reads the wrapper's
     per-turn PID/PGID file and queries `ps -o pid,pgid` (or
     equivalent) to confirm the entire group is gone.
  3. The test fixture intentionally arranges a case where the
     wrapper's PID and PGID would differ at first invocation
     (e.g., start Codex through the wrapper from a parent shell that
     already established a process group), and asserts the wrapper's
     `PID == PGID` self-check recovers correctly — i.e. that
     `kill 0` after the recovery signals exactly the wrapper's
     group and nothing outside it.
- A second integration test MUST cover the network-drop path: kill the
  SSH session from the AgentDesk side without sending Ctrl-C, and
  assert the same "no descendants" property holds after the wrapper's
  SIGHUP handler runs.

Without **both** integration tests passing, the gate stays off
regardless of `remote_ssh_enabled` — this is what
`PREREQUISITES_SATISFIED` in `src/services/codex_remote_policy.rs`
gates.

This ADR explicitly does **not** introduce a remote tmux session for
Codex. `execute_streaming_remote_tmux` remains a stub because remote
tmux fragments the cancel story (the SSH session can drop while the
tmux session — and Codex — keeps running on the remote host with no
owner). Reviving remote tmux requires a separate ADR.

## Failure / blast radius

- **SSH session drops mid-turn (network).** AgentDesk MUST treat the
  dropped channel identically to a cancel: emit a final
  `StreamMessage::Error` with `runtime_kind=remote-direct`, surface the
  drop to the operator, and abandon the turn. AgentDesk MUST NOT
  auto-reconnect. The remote Codex child receives SIGHUP via the PTY
  closing; whether it survives is the remote wrapper's problem.
- **Host down at dispatch.** Connection failure is a hard fail; the
  turn does not silently fall back to local execution. Falling back
  would surprise the operator (different filesystem, different
  toolchain) and is explicitly out of scope.
- **Host compromise.** A compromised remote host can read the prompt
  for the current turn, the working directory contents on that host,
  and any environment exported by the remote shell. It cannot read
  AgentDesk's secrets (see "Trust boundary") and it cannot call back
  into AgentDesk's control plane because there is no inbound path.
  Recovery is operator-level: remove the host from the allow-list,
  rotate any project-level credentials that live on the remote host,
  and treat any code the remote turn wrote as untrusted until
  reviewed.
- **Allow-list drift.** If `agentdesk.yaml` is edited to add a host
  while the process is running, the allow-list MUST be re-read at
  config-reload time. AgentDesk's existing config-reload path is the
  enforcement point; this ADR does not introduce a new one.

## Non-goals

- Multi-hop SSH (`ssh -J`, jump hosts, bastion chains). Single hop
  only.
- Running arbitrary shell commands on the remote host outside the
  Codex CLI invocation. This is "remote Codex", not "remote shell".
- Inbound connections from the remote host to AgentDesk (reverse
  tunnels, callback webhooks, remote MCP servers calling local
  AgentDesk).
- File sync (rsync, sftp) of project state between AgentDesk and the
  remote host. The remote host owns its own working directory.
- Per-turn dynamic host selection from a PMD policy, an LLM, or the
  Discord operator. Targeting is static config.
- Remote tmux for Codex (see "Cancel semantics").
- Enabling remote SSH for any provider other than Codex. Claude,
  Gemini, Qwen, and OpenCode keep their stubs.
- Storing SSH credentials in `agentdesk.yaml`. Cleartext password and
  passphrase-protected key paths are explicitly forbidden at
  config-load time when the gate is on.

## Open questions

- Concrete russh vs. `ssh` subprocess implementation choice. russh
  gives a cleaner cancel-via-channel-close story; subprocess `ssh`
  gives free ssh-agent / known_hosts / config-file behavior. This ADR
  leans toward subprocess `ssh` for the initial implementation
  because reusing the operator's existing SSH config is more important
  than crate-level control, but the choice is deferred to the
  implementation issue.
- Whether `default_path` on a `remote_hosts` entry should be a
  hard requirement or fall back to `$HOME`. Defaulting to `$HOME` is
  hostile to operators who keep multiple repos per host; this ADR
  leans toward hard-required, but the decision is deferred.
- Telemetry: this ADR keeps `runtime_kind=remote-direct` (from
  `docs/codex-exec-policy.md`) as the observability label. Whether to
  add a `remote_host=<name>` low-cardinality field on the existing
  span is deferred to the implementation issue.

## Known follow-ups

The ADR does **not** build remote SSH execution. The following remain
explicit follow-ups gated behind this document:

1. Replace `services::remote_stub` with a real `services::remote`
   module that honors the auth contract above (ssh-agent + strict
   known_hosts, no password/passphrase). Tracked separately.
2. Add `providers.codex.remote_hosts: [...]` deserialization plus the
   per-`AgentDef` `codex.remote_host` field. Tracked separately.
3. Implement `execute_streaming_remote_direct` end-to-end with the
   PTY-bound cancel path and the integration test described in
   "Cancel semantics". Tracked separately.
4. Reject `RemoteAuth::Password` and `RemoteAuth::KeyFile` at
   config-load time when `remote_ssh_enabled` is `true`. Tracked
   separately.

Per this ADR, the gate `providers.codex.remote_ssh_enabled` lands now
and defaults `false`. Flipping it to `true` while the follow-ups above
are not in place is a **hard bootstrap error**, not a warning — see
`src/services/codex_remote_policy.rs::PREREQUISITES_SATISFIED`. A
warn-only gate would become a persisted "enabled" signal that a
partial future implementation could silently honor; the hard-fail
removes that risk and forces the implementation PR to flip
`PREREQUISITES_SATISFIED` to `true` in the same change that wires
everything up.
