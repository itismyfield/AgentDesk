# Self-Hosted GitHub Actions Runner (macOS)

Goal: reduce queue time on macOS CI jobs by hosting them on the operator's
`mac-mini` (Apple Silicon). Self-hosted macOS execution is **opt-in** and is
isolated to `.github/workflows/ci-macos-trusted.yml`, which has no
`pull_request` or `pull_request_target` trigger. PR workflows and nightly
macOS stay on GitHub-hosted runners.

> **Status:** docs + workflow toggle only. Runner registration must be
> performed by the operator on the host machine (requires a GitHub-issued
> registration token).

---

## 1. Pre-requisites (on `mac-mini`)

1. **Xcode Command Line Tools**
   ```bash
   xcode-select --install
   ```
2. **Homebrew packages** (the CI workflow currently installs `opus` /
   `pkg-config` per-job; install them once so cold runs are fast):
   ```bash
   brew install opus pkg-config sccache jq coreutils gnu-tar
   ```
3. **Rust toolchain manager** — actions install pinned Rust per-run via
   `dtolnay/rust-toolchain`, so a pre-installed `rustup` is **not** required.
   Installing it anyway keeps interactive debugging convenient:
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
   ```
4. **Disk budget**: reserve ~40 GB for `~/actions-runner/_work` (checkouts +
   target dirs across jobs). See [§5 Maintenance](#5-maintenance) for cleanup.
5. **Network**: the runner needs outbound HTTPS to `github.com` and
   `*.actions.githubusercontent.com`. No inbound ports are required.
6. **Org runner group**: create an org-level runner group for trusted macOS
   CI, restrict repository access to `itismyfield/AgentDesk`, and do not share
   it with other repos. The workflow requires the group name via
   `vars.MACOS_RUNNER_GROUP` before it will route to self-hosted labels.

---

## 2. Fetch a registration token (GitHub UI)

Tokens are short-lived (~1 hour) and must be regenerated on every
register/deregister. The operator fetches them at:

```
https://github.com/<owner>/<repo>/settings/actions/runners/new
```

For this repo, create or select the org runner group first, then add the
runner from **Settings → Actions → Runners → New self-hosted runner →
macOS / ARM64**. Copy the `--token` value from the displayed `./config.sh`
command.

> Do **not** commit the token. Do **not** share it in chat / PRs. It is
> equivalent to a write credential for the runner namespace.

---

## 3. Register and run as a launchd service

Match the launchd pattern used elsewhere in AgentDesk
(`dcserver` / preview-bridge): user-level `LaunchAgents`, auto-restart,
stdout/stderr to `~/Library/Logs/agentdesk/`.

### 3.1 Download and configure

```bash
RUNNER_VERSION="2.321.0"   # check https://github.com/actions/runner/releases
mkdir -p ~/actions-runner && cd ~/actions-runner

curl -O -L "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-osx-arm64-${RUNNER_VERSION}.tar.gz"
tar xzf "./actions-runner-osx-arm64-${RUNNER_VERSION}.tar.gz"

# Replace <REG_TOKEN> with the token from §2.
./config.sh \
  --url    https://github.com/itismyfield/AgentDesk \
  --token  <REG_TOKEN> \
  --name   agentdesk-mac-mini \
  --labels self-hosted,macOS,arm64,agentdesk-macos,agentdesk-mac-mini \
  --work   _work \
  --unattended \
  --replace
```

The labels are deliberate — the workflow toggle (§4) targets the *full label
set* so we can add `mac-book` later without changing the workflow. Note that
both a **fleet** label (`agentdesk-macos`) and a **host** label
(`agentdesk-mac-mini`) are applied; the workflow variable picks which one
matters for routing:

| Label | Purpose |
|-------|---------|
| `self-hosted` | Default GitHub category. |
| `macOS` | OS family. |
| `arm64` | Architecture (Apple Silicon). |
| `agentdesk-macos` | **Fleet** label. Apply to every AgentDesk macOS runner. Use this in `MACOS_RUNNER` when you want either host to be eligible. |
| `agentdesk-mac-mini` | **Host** label. Use in `MACOS_RUNNER` to pin a job to this specific machine. |

Update the `--labels` flag in §3.1 above to match:
`self-hosted,macOS,arm64,agentdesk-macos,agentdesk-mac-mini`. The
`config.sh` invocation in this doc has been written with both labels for
exactly this reason.

> **Two label formats — don't confuse them.** `config.sh --labels` takes a
> comma-delimited string (shown above). `runs-on: ${{ fromJSON(...) }}` in
> the workflow needs a JSON array string in `vars.MACOS_RUNNER` (§4). The
> labels themselves are identical; only the delimiter differs.

### 3.2 launchd service (user LaunchAgent)

Create `~/Library/LaunchAgents/com.agentdesk.ghrunner.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>            <string>com.agentdesk.ghrunner</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>-lc</string>
    <string>cd "$HOME/actions-runner" && exec ./run.sh</string>
  </array>
  <key>RunAtLoad</key>        <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key> <false/>
    <key>Crashed</key>        <true/>
  </dict>
  <key>ThrottleInterval</key> <integer>30</integer>
  <key>ProcessType</key>      <string>Interactive</string>
  <key>StandardOutPath</key>  <string>/Users/REPLACE_ME/Library/Logs/agentdesk/ghrunner.out.log</string>
  <key>StandardErrorPath</key><string>/Users/REPLACE_ME/Library/Logs/agentdesk/ghrunner.err.log</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>           <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    <key>HOME</key>           <string>/Users/REPLACE_ME</string>
    <key>SCCACHE_DIR</key>    <string>/Users/REPLACE_ME/.cache/sccache</string>
    <key>SCCACHE_CACHE_SIZE</key><string>20G</string>
  </dict>
</dict>
</plist>
```

Replace `REPLACE_ME` with `whoami` output, then load:

```bash
mkdir -p ~/Library/Logs/agentdesk ~/.cache/sccache
launchctl unload ~/Library/LaunchAgents/com.agentdesk.ghrunner.plist 2>/dev/null || true
launchctl load   ~/Library/LaunchAgents/com.agentdesk.ghrunner.plist
launchctl list | grep ghrunner   # PID column should be a number, not "-"
```

Verify on GitHub: **Settings → Actions → Runners** → `agentdesk-mac-mini`
should show `Idle`.

---

## 4. Opt-in from the workflow side

Self-hosted macOS routing is owned by `ci-macos-trusted.yml` only. That
workflow triggers on trusted `push`, `workflow_dispatch`, and `merge_group`
events, never on PR events. A setup job validates the JSON labels and runner
group before any self-hosted job is queued; malformed values fail with a
normal workflow error instead of breaking workflow startup with `fromJSON`.

**Default behaviour:** `vars.MACOS_RUNNER` is unset → the trusted macOS job
runs on GitHub-hosted `macos-latest`.

**PR and nightly behaviour:** `ci-pr.yml` and `ci-nightly.yml` do not read
`MACOS_RUNNER`; they cannot route to self-hosted macOS by variable change.

To opt in *after* the runner is live and `Idle`:

1. GitHub organization settings → **Actions → Runner groups**:
   confirm the trusted macOS runner group exists and is explicitly allowed
   for `itismyfield/AgentDesk`.
2. GitHub repo → **Settings → Secrets and variables → Actions → Variables**.
3. Add repo variable `MACOS_RUNNER_GROUP` with the exact group name, for
   example `AgentDesk trusted macOS`.
4. Add repo variable `MACOS_RUNNER`. Two recommended values:
   - **Pinned to mac-mini** (only this host can pick up the job):
     ```json
     ["self-hosted","macOS","arm64","agentdesk-mac-mini"]
     ```
   - **Any AgentDesk macOS runner** (recommended once mac-book exists too):
     ```json
     ["self-hosted","macOS","arm64","agentdesk-macos"]
     ```
   The double quotes and brackets are required — `MACOS_RUNNER` must be a
   JSON array, and `runs-on` selects a runner that matches **all** labels in
   the array (intersection). Do **not** use
   `["self-hosted","macOS","arm64"]` alone — without a project label that
   pattern would match any visible self-hosted macOS ARM64 runner, including
   ones registered for other purposes later.
5. Trigger **CI macOS Trusted** with `workflow_dispatch`. The self-hosted
   job logs `runner.name` before checkout so routing is easy to confirm. If
   it doesn't pick up the expected runner, see [§7 Failure modes](#7-failure-modes).

To roll back: delete `vars.MACOS_RUNNER`. The next trusted macOS run goes
back to `macos-latest`. No code change needed. Leave `MACOS_RUNNER_GROUP`
set only if the org group remains valid.

> **Why a variable and not a secret?** Labels are not sensitive and we want
> them visible in logs for debugging.

> **Validating the JSON format locally before saving:**
> `printf '%s' '["self-hosted","macOS","arm64","agentdesk-mac-mini"]' | python3 -c "import json,sys; print(json.load(sys.stdin))"`
> should print a Python list of four strings.

---

## 5. Maintenance

| Task | Command / location |
|------|-------------------|
| Runner stdout/stderr | `~/Library/Logs/agentdesk/ghrunner.{out,err}.log` |
| Per-run diagnostics | `~/actions-runner/_diag/` |
| Restart runner | `launchctl kickstart -k gui/$(id -u)/com.agentdesk.ghrunner` |
| Stop runner | `launchctl unload ~/Library/LaunchAgents/com.agentdesk.ghrunner.plist` |
| Update runner version | stop → re-extract tarball → reload (token re-fetch not required) |
| Deregister | `cd ~/actions-runner && ./config.sh remove --token <DEREG_TOKEN>` (token from same UI page) |
| sccache cache dir | `~/.cache/sccache` (size cap via `SCCACHE_CACHE_SIZE`) |
| sccache stats | `sccache --show-stats` |
| Workspace cleanup | `rm -rf ~/actions-runner/_work/*` while runner is `Idle` |
| Disk usage check | `du -sh ~/actions-runner/_work ~/.cache/sccache ~/Library/Logs/agentdesk` |

---

## 6. Security notes

GitHub Actions self-hosted runners on `pull_request` from **forks** execute
attacker-controlled code with the runner user's privileges. This is the
canonical compromise vector and is called out in
[GitHub's docs](https://docs.github.com/en/actions/hosting-your-own-runners/managing-self-hosted-runners/about-self-hosted-runners#self-hosted-runner-security).

**Policy for this repo:**

1. **`pull_request*` jobs never run on self-hosted — by workflow structure,
   not by fork check.** Because a `pull_request` workflow file is supplied
   by the PR head (i.e. PR-controlled), any workflow-local fork comparison
   can be removed or rewritten by the PR itself and is therefore not a real
   isolation boundary. The robust approach we use:

   - `ci-pr.yml` has no `MACOS_RUNNER`, runner-group, or self-hosted macOS
     routing expression.
   - `ci-nightly.yml` always uses GitHub-hosted `macos-latest`.
   - `ci-macos-trusted.yml` is the only workflow that reads `MACOS_RUNNER`
     and `MACOS_RUNNER_GROUP`, and it has no `pull_request` or
     `pull_request_target` trigger.
   - `scripts/check-ci-runner-hardening.sh` fails CI if a PR-triggered
     workflow regains self-hosted macOS routing or if `MACOS_RUNNER` appears
     outside the trusted workflow.

   This means: fork contributors cannot reach the self-hosted runner via
   PRs at all. Trusted events (`push`, `workflow_dispatch`, `merge_group`)
   are the only self-hosted entry points.

   **Required pre-flight before setting `MACOS_RUNNER`** (operator
   responsibility, in addition to the workflow-level event gate):

   a. **Settings → Actions → General → "Approval for outside collaborators"**:
      set to **"Require approval for all outside collaborators"** (or
      stricter, e.g. "first-time contributors who are new to GitHub"). A
      maintainer must click "Approve and run" on every fork PR's first run.
      Even though fork PRs cannot select the self-hosted runner under the
      event gate, this setting also protects any unrelated future
      self-hosted lanes added to the repo.

   b. **Organization settings → Actions → Runner groups**: create/select the
      trusted macOS runner group, restrict repository access to
      `itismyfield/AgentDesk`, and set `vars.MACOS_RUNNER_GROUP` to the
      exact group name. The workflow fails before queuing self-hosted work
      when `MACOS_RUNNER` is set without a group.

   **Until (a) and (b) are confirmed, do not set `MACOS_RUNNER`.**
2. Acceptable triggers for the self-hosted runner: `push` to branches owned
   by this repo, `workflow_dispatch`, `merge_group`. Do **not** route
   `pull_request` (same-repo or fork) or `pull_request_target` to the
   self-hosted runner — both event shapes carry PR-controlled or
   elevated-permission risk that workflow-local checks cannot fully contain.
3. Runner runs as the operator's user — **not** root. Do not `sudo` from
   within steps. Keep the runner dir on the same volume as the operator's
   home so file ownership is unsurprising.
4. Do not register the same runner against multiple repos with the current
   labels. Shared runners must stay behind the org runner group allow-list
   and must not use broad labels such as `["self-hosted","macOS","arm64"]`
   without an AgentDesk-specific fleet or host label.

### 6.5 Incident response

If the runner host is suspected compromised, assume `~/actions-runner/.runner`
and `~/actions-runner/.credentials` were copied. Unloading launchd is not
enough because those credentials can keep receiving jobs from another host.

1. GitHub → **Settings → Actions → Runners → agentdesk-mac-mini → ⋯ →
   Remove**. Use the force-remove UI path if the host is offline.
2. Delete repo variable `MACOS_RUNNER` immediately. Leave
   `MACOS_RUNNER_GROUP` only if the group remains valid and empty.
3. Revoke any PATs, SSH keys, signing credentials, or deploy tokens cached
   on the host.
4. Rotate the runner by deleting `~/actions-runner`, registering a fresh
   runner with a new token, and confirming the org runner group allow-list
   before re-setting `MACOS_RUNNER`.

---

## 7. Failure modes

| Symptom | What happens | Operator action |
|---------|--------------|-----------------|
| Runner offline (launchd dead, host off, network down) | Jobs **queue indefinitely** on the self-hosted label. GitHub does **not** auto-fall-back to hosted. | Either: (a) bring the runner back, or (b) delete `vars.MACOS_RUNNER` to revert to `macos-latest` and re-run. |
| `pull_request` macOS job didn't pick up self-hosted runner (same-repo PR or fork PR) | Expected. PR workflows do not read `MACOS_RUNNER`; self-hosted is isolated to **CI macOS Trusted**. | No action. To exercise self-hosted on a branch, push directly to the branch in this repo or trigger `workflow_dispatch`. |
| `MACOS_RUNNER` rejected by the resolve job | The variable is missing required labels, not valid JSON, or `MACOS_RUNNER_GROUP` is unset. | Re-save `MACOS_RUNNER` as a JSON array and set `MACOS_RUNNER_GROUP` to the exact org runner group name. |
| Job queues forever even though the runner is online | The selected runner does not have every label in `MACOS_RUNNER`, or it is not in the configured runner group. | Run `gh api /repos/itismyfield/AgentDesk/actions/runners --jq '.runners[] | {name,status,labels:[.labels[].name]}'` and confirm the runner is `online`, in the group, and has every configured label. |
| Runner online but stuck on prior job | New jobs queue behind it. | `launchctl kickstart -k …` to restart; cancel any zombie job from GitHub UI. |
| Token expired during `config.sh` | `config.sh` exits non-zero with `Http response code: NotFound`. | Re-fetch token (§2). Tokens last ~1h. |
| Brew dep drift (e.g. `opus` removed) | Cargo build fails at link time. | Re-run §1 brew install line. Add the package to the launchd `EnvironmentVariables` `PATH` if it lands outside `/opt/homebrew/bin`. |
| `_work` dir fills the disk | Jobs fail at `actions/checkout`. | `rm -rf ~/actions-runner/_work/*` while `Idle`. Add a cron (follow-up §8.c). |

> The opt-in toggle is the kill switch: if anything is on fire and you can't
> reach the host, deleting `vars.MACOS_RUNNER` restores hosted-runner CI in
> the next workflow run.

---

## 8. Follow-up work (parallel migration)

These are tracked here rather than as separate issues until the operator
confirms direction. Each item is independent.

a. **Cache directory persistence.** GitHub's `actions/cache@v4` still works
   on self-hosted but writes to its own scratch dir. For maximum hit rate,
   add a step that hard-links / rsyncs to a persistent dir on the runner
   (e.g. `~/cache/cargo-registry`) and back. See `runner.tool_cache`.

b. **Cleanup cron.** A launchd `StartCalendarInterval` agent that runs daily
   at 04:00 KST and prunes `_work/*` older than 3 days plus `sccache --zero-stats`
   if size > 18 GB.

c. **Monitoring.** Polling `gh api /repos/{owner}/{repo}/actions/runners`
   from the existing dcserver health loop and surfacing `status != online`
   to the operator's Discord channel.

d. **`mac-book` as a second runner.** Repeat §3 with
   `--name agentdesk-mac-book --labels self-hosted,macOS,arm64,agentdesk-macos,agentdesk-mac-book`.
   Apply the same shared **fleet** label `agentdesk-macos` so the workflow
   variable `MACOS_RUNNER=["self-hosted","macOS","arm64","agentdesk-macos"]`
   lets either host pick up the job. To pin to one host, swap the fleet
   label for the host label (`agentdesk-mac-mini` or `agentdesk-mac-book`).
